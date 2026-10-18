#!/usr/bin/env bash
# Installs what the Firefox engine's integration tests need on a Debian or
# Ubuntu machine (a CI runner, WSL2, a devcontainer): the pinned Camoufox
# build, Xvfb with setxkbmap, Mesa's software OpenGL and the libraries a
# Firefox links against.
#
# The version, the release and the checksum of the release archive are read
# from deploy/docker/sidecar.Dockerfile, so tests and image run the same
# build. The archive is fetched by its exact address, never through the
# launcher's own download (`camoufox-js fetch`), which installs another
# build. A machine that already has the pinned build is left alone; another
# build in the same place is replaced, so that a machine that ran the
# earlier pin moves to this one.
#
#   KAGE_CAMOUFOX_DIR   where the build is unpacked, /opt/camoufox when not
#                       set. The engine looks there too; tell it about
#                       another place with SIDECAR_CAMOUFOX_DIR.
#
# No sound server is installed: the tests run without one
# (SIDECAR_AUDIO=none). Install `pulseaudio` to try SIDECAR_AUDIO=pulse.
set -euo pipefail
cd "$(dirname "$0")/.."

dockerfile="deploy/docker/sidecar.Dockerfile"
version="$(sed -n 's/^ARG CAMOUFOX_VERSION=//p' "${dockerfile}")"
release="$(sed -n 's/^ARG CAMOUFOX_RELEASE=//p' "${dockerfile}")"
checksum="$(sed -n 's/^ARG CAMOUFOX_SHA256=//p' "${dockerfile}")"
if [ -z "${version}" ] || [ -z "${release}" ] || [ -z "${checksum}" ]; then
    echo "error: ${dockerfile} does not name CAMOUFOX_VERSION, CAMOUFOX_RELEASE and CAMOUFOX_SHA256" >&2
    exit 1
fi

build="${version}-${release}"
dir="${KAGE_CAMOUFOX_DIR:-/opt/camoufox}"
camoufox="${dir}/camoufox-bin"
# What the engine reads to know which build it runs, and refuses to differ.
stamp="{\"version\":\"${version}\",\"release\":\"${release}\"}"

# What the browser itself says it is; nothing when it cannot start, which is
# what a missing system library looks like.
reported() {
    "${camoufox}" --version 2> /dev/null | sed -E 's/^.* //' || true
}

sudo=""
if [ "$(id -u)" -ne 0 ]; then
    sudo="sudo"
fi

unpacked=""
if [ -x "${camoufox}" ] && [ -r "${dir}/version.json" ] \
    && [ "$(tr -d '[:space:]' < "${dir}/version.json")" = "${stamp}" ]; then
    unpacked=1
fi
if [ -n "${unpacked}" ] && [ "$(reported)" = "${build}" ] \
    && command -v Xvfb > /dev/null && command -v setxkbmap > /dev/null; then
    echo "Camoufox ${build}, Xvfb and setxkbmap are already installed"
    exit 0
fi

if [ "$(uname -s)" != "Linux" ] || ! command -v apt-get > /dev/null; then
    echo "error: the sidecar's browser tests need Linux with apt (a CI runner, WSL2 or a devcontainer)" >&2
    exit 1
fi
if [ -e "${dir}" ] && [ ! -e "${camoufox}" ] && [ -n "$(ls -A "${dir}" 2> /dev/null)" ]; then
    echo "error: ${dir} exists and holds no Camoufox build; it is not replaced. Name another place in KAGE_CAMOUFOX_DIR." >&2
    exit 1
fi

echo "==> installing Xvfb, setxkbmap, Mesa and the libraries Camoufox links against"
${sudo} apt-get update --quiet
# What the browser's binaries name as needed: the list of the image. Debian
# 13 and Ubuntu 24.04 carry four of them under a new name (64-bit time).
libraries=(
    libcairo-gobject2 libcairo2 libdbus-1-3 libfontconfig1 libfreetype6 libgdk-pixbuf-2.0-0
    libpango-1.0-0 libpangocairo-1.0-0 libx11-6 libx11-xcb1 libxcb-shm0 libxcb1 libxcomposite1
    libxcursor1 libxdamage1 libxext6 libxfixes3 libxi6 libxrandr2 libxrender1
)
for library in libasound2 libatk1.0-0 libglib2.0-0 libgtk-3-0; do
    if apt-cache show "${library}t64" > /dev/null 2>&1; then
        libraries+=("${library}t64")
    else
        libraries+=("${library}")
    fi
done
${sudo} env DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends \
    unzip xvfb x11-xkb-utils libgl1-mesa-dri libegl1 "${libraries[@]}"

if [ -z "${unpacked}" ]; then
    # KAGE_CAMOUFOX_ARCHIVE names a place to keep the archive between runs
    # (CI's cache): a copy there is taken when it is the pinned archive, and
    # anything else there is replaced by a fresh download.
    archive="${KAGE_CAMOUFOX_ARCHIVE:-}"
    if [ -z "${archive}" ]; then
        archive="$(mktemp --suffix=.zip)"
        trap 'rm -f "${archive}"' EXIT
    fi
    if echo "${checksum}  ${archive}" | sha256sum --check --strict --status 2> /dev/null; then
        echo "==> Camoufox ${build} from ${archive}"
    else
        echo "==> downloading Camoufox ${build}"
        mkdir -p "$(dirname "${archive}")"
        curl --fail --silent --show-error --location --retry 3 --output "${archive}" \
            "https://github.com/daijro/camoufox/releases/download/v${build}/camoufox-${build}-lin.x86_64.zip"
    fi
    echo "${checksum}  ${archive}" | sha256sum --check --strict

    echo "==> unpacking Camoufox ${build} into ${dir}"
    # Another build in the same place goes first: files of two builds must
    # not mix.
    ${sudo} rm -rf "${dir}"
    ${sudo} install --directory "${dir}"
    # unzip exits 1 for warnings with every file extracted (Debian's warns
    # that the 156 archive's Hiragino font names differ between the local
    # and the central header, and takes the central one); above 1 is an
    # error.
    ${sudo} unzip -q "${archive}" -d "${dir}" || [ "$?" -eq 1 ]
    ${sudo} chmod -R a+rX,go-w "${dir}"
    printf '%s\n' "${stamp}" | ${sudo} tee "${dir}/version.json" > /dev/null
fi

if [ "$(reported)" != "${build}" ]; then
    echo "error: ${camoufox} does not start as Camoufox ${build}; run '${camoufox} --version' to see why" >&2
    exit 1
fi
"${camoufox}" --version
