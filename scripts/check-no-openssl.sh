#!/usr/bin/env bash
# Fails when OpenSSL is anywhere in the Rust dependency graph.
#
# wreq links BoringSSL. A second TLS library exporting the same symbols in one
# binary breaks the build or, worse, the TLS fingerprint (spec risk R6), so the
# rule is absolute: `cargo tree -i openssl-sys` must be empty.
set -euo pipefail
cd "$(dirname "$0")/.."

banned=(openssl-sys)
status=0

for crate in "${banned[@]}"; do
    # The lockfile covers every target and every feature of the workspace, so
    # this catches a dependency that only appears on another platform.
    if grep -Eq "^name = \"${crate}\"$" Cargo.lock; then
        echo "error: ${crate} is in Cargo.lock" >&2
        status=1
    fi

    # The check exactly as the spec states it, widened to all targets,
    # features and edge kinds.
    stderr_file="$(mktemp)"
    set +e
    tree="$(cargo tree --workspace --all-features --target all --edges all --locked \
        --invert "${crate}" 2> "${stderr_file}")"
    code=$?
    set -e
    stderr="$(cat "${stderr_file}")"
    rm -f "${stderr_file}"

    if [ "${code}" -eq 0 ]; then
        if [ -n "${tree}" ]; then
            echo "error: ${crate} is in the dependency graph:" >&2
            echo "${tree}" >&2
            status=1
        fi
    elif ! grep -q "did not match any packages" <<< "${stderr}"; then
        # Anything other than "not in the graph" means the check did not run.
        echo "error: cargo tree failed while checking ${crate}:" >&2
        echo "${stderr}" >&2
        status=1
    fi
done

if [ "${status}" -eq 0 ]; then
    echo "ok: no ${banned[*]} in the Rust dependency graph"
fi
exit "${status}"
