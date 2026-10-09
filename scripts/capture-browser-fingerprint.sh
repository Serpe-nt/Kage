#!/usr/bin/env bash
# Records what the Chromium engine's real browser shows the in-repo
# fingerprint echo, and writes it to
# tests/fixtures/fingerprints/browser/chromium-<major>.json (spec §6.1 step 4,
# follow-up F5 in docs/02-plan.md).
#
# Run it when the pinned Chrome moves: the capture decides whether the http
# path may present as that major (VERIFIED_MAJORS in kage-http) or the
# engine stays browser-only (BROWSER_ONLY_MAJORS). Everything stays on this
# machine: the echo listens on loopback and the browser reaches it through
# a local proxy that relays nothing else.
#
# Needs the pinned Chrome and Xvfb (`just setup-chrome`), `certutil`
# (libnss3-tools), `openssl`, and a user that is not root.
set -euo pipefail
cd "$(dirname "$0")/.."

host="echo.kage.test"
out_dir="tests/fixtures/fingerprints/browser"
work="$(mktemp -d)"
echo_pid=""

cleanup() {
    if [ -n "${echo_pid}" ]; then
        kill "${echo_pid}" 2> /dev/null || true
    fi
    rm -rf "${work}"
}
trap cleanup EXIT

echo "==> a throwaway certificate authority and a certificate for ${host}"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 2 \
    -subj "/CN=kage capture authority" -keyout "${work}/ca.key" -out "${work}/ca.pem" 2> /dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -subj "/CN=${host}" -keyout "${work}/key.pem" -out "${work}/server.csr" 2> /dev/null
printf 'subjectAltName=DNS:%s,DNS:h1.%s\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\n' \
    "${host}" "${host}" > "${work}/ext.cnf"
openssl x509 -req -in "${work}/server.csr" -CA "${work}/ca.pem" -CAkey "${work}/ca.key" \
    -CAcreateserial -days 2 -extfile "${work}/ext.cnf" -out "${work}/cert.pem" 2> /dev/null

echo "==> building kage and the sidecar"
cargo build --locked --package kage
(cd sidecar && pnpm run build > /dev/null)

# A fixed port: it shows in the `origin` and `referer` values of the capture,
# and a capture should only change when the browser does.
port="${KAGE_CAPTURE_PORT:-18443}"
echo "==> kage echo serve on 127.0.0.1:${port}"
"${CARGO_TARGET_DIR:-target}/debug/kage" echo serve --bind "127.0.0.1:${port}" \
    --cert "${work}/cert.pem" --key "${work}/key.pem" > "${work}/echo.log" 2>&1 &
echo_pid=$!
for _ in $(seq 1 50); do
    if curl --silent --output /dev/null --cacert "${work}/ca.pem" \
        --resolve "${host}:${port}:127.0.0.1" "https://${host}:${port}/"; then
        break
    fi
    sleep 0.2
done

echo "==> the engine's browser against the echo"
mkdir -p "${out_dir}"
node sidecar/packages/engine-chromium/dist/testing/capture-echo-bin.js \
    --host "${host}" --port "${port}" --ca "${work}/ca.pem" --out-dir "${out_dir}"

echo "==> done. Review the diff of ${out_dir}/, then run:"
echo "    cargo test --package kage-http --test fingerprints every_browser_capture_is_accounted_for"
