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
# build. A machine that already has the pinned build is left alone.
#
# The build then gets the Juggler backport (ADR 0010,
# scripts/patch-camoufox-juggler.mjs): the same bytes the image runs. A
# build unpacked before the backport existed is patched in place; the engine
# refuses one that is not. That step needs `node`.
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
omni="$(sed -n 's/^ARG CAMOUFOX_OMNI_SHA256=//p' "${dockerfile}")"
patched="$(sed -n 's/^ARG CAMOUFOX_OMNI_PATCHED_SHA256=//p' "${dockerfile}")"
if [ -z "${version}" ] || [ -z "${release}" ] || [ -z "${checksum}" ]; then
    echo "error: ${dockerfile} does not name CAMOUFOX_VERSION, CAMOUFOX_RELEASE and CAMOUFOX_SHA256" >&2
    exit 1
fi
if [ -z "${omni}" ] || [ -z "${patched}" ]; then
    echo "error: ${dockerfile} does not name CAMOUFOX_OMNI_SHA256 and CAMOUFOX_OMNI_PATCHED_SHA256" >&2
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

# Whether the build's archive holds the Juggler backport; says nothing.
backported() {
    node scripts/patch-camoufox-juggler.mjs "${dir}" --to "${patched}" --check > /dev/null 2>&1
}

sudo=""
if [ "$(id -u)" -ne 0 ]; then
    sudo="sudo"
fi

# Applies the Juggler backport. The tool refuses an archive that is neither
# the pinned build's nor the patched one. A build that belongs to root is
# patched under sudo, which has a PATH of its own: node goes by its path.
backport() {
    echo "==> applying the Juggler backport to ${dir}/omni.ja"
    if [ -w "${dir}" ] && [ -w "${dir}/omni.ja" ]; then
        node scripts/patch-camoufox-juggler.mjs "${dir}" --from "${omni}" --to "${patched}"
    else
        ${sudo} "$(command -v node)" scripts/patch-camoufox-juggler.mjs "${dir}" \
            --from "${omni}" --to "${patched}"
    fi
}

if ! command -v node > /dev/null; then
    echo "error: the Juggler backport needs node (\`just setup-node\` installs the pinned one)" >&2
    exit 1
fi

unpacked=""
if [ -x "${camoufox}" ] && [ -r "${dir}/version.json" ] \
    && [ "$(tr -d '[:space:]' < "${dir}/version.json")" = "${stamp}" ]; then
    unpacked=1
fi
if [ -n "${unpacked}" ] && [ "$(reported)" = "${build}" ] \
    && command -v Xvfb > /dev/null && command -v setxkbmap > /dev/null; then
    if backported; then
        echo "Camoufox ${build} with the Juggler backport, Xvfb and setxkbmap are already installed"
    else
        # Everything is there but the backport: a build unpacked before it existed.
        backport
    fi
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
    archive="$(mktemp --suffix=.zip)"
    trap 'rm -f "${archive}"' EXIT
    echo "==> downloading Camoufox ${build}"
    curl --fail --silent --show-error --location --retry 3 --output "${archive}" \
        "https://github.com/daijro/camoufox/releases/download/v${build}/camoufox-${build}-lin.x86_64.zip"
    echo "${checksum}  ${archive}" | sha256sum --check --strict

    echo "==> unpacking Camoufox ${build} into ${dir}"
    # Another build in the same place goes first: files of two builds must
    # not mix.
    ${sudo} rm -rf "${dir}"
    ${sudo} install --directory "${dir}"
    ${sudo} unzip -q "${archive}" -d "${dir}"
    ${sudo} chmod -R a+rX,go-w "${dir}"
    printf '%s\n' "${stamp}" | ${sudo} tee "${dir}/version.json" > /dev/null
fi

if ! backported; then
    backport
fi

if [ "$(reported)" != "${build}" ]; then
    echo "error: ${camoufox} does not start as Camoufox ${build}; run '${camoufox} --version' to see why" >&2
    exit 1
fi
"${camoufox}" --version
