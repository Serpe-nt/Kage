#!/usr/bin/env bash
# QA round two, topic tls: make every persona's browser profile of a stack
# trust the private CA, by writing it into the profile's NSS database on the
# personas volume (as the sidecar's capture-echo tools do). Run in kage-qa2
# (it has certutil) after `stack.sh up`, ideally before a persona's first
# browser start (a running browser may not reread the database).
#
#   bash /scripts/qa/infra/tls/trust.sh STACK [CA_PEM]
#
# chromium: <personas>/<profile_dir>/home/.pki/nssdb   (Chrome on Linux: $HOME/.pki/nssdb)
# firefox:  <personas>/<profile_dir>/user-data         (the profile's cert9.db)
# Files are owned by 65532:65532, the sidecars' user, mode 0700/0600.
set -euo pipefail
stack="${1:?stack}"
ca="${2:-/scripts/qa/infra/tls/certs/ca.pem}"
volume="$(docker volume inspect "qa-${stack}_personas" -f '{{.Mountpoint}}')"
rows="$(bash /scripts/qa/stack.sh sql "${stack}" "select engine, profile_dir from personas where retired_at is null")"
while IFS='|' read -r engine dir; do
    [ -n "${engine}" ] || continue
    case "${engine}" in
    chromium) db="${volume}/${dir}/home/.pki/nssdb" ;;
    firefox) db="${volume}/${dir}/user-data" ;;
    *) continue ;;
    esac
    mkdir -p "${db}"
    if [ ! -f "${db}/cert9.db" ]; then certutil -d "sql:${db}" -N --empty-password; fi
    certutil -d "sql:${db}" -D -n kage-qa-tls-root 2> /dev/null || true
    certutil -d "sql:${db}" -A -t 'C,,' -n kage-qa-tls-root -i "${ca}"
    chown -R 65532:65532 "${volume}/${engine}" 2> /dev/null || chown -R 65532:65532 "${volume}/${dir}"
    chmod 700 "${db}"
    echo "${engine} ${dir}: $(certutil -d "sql:${db}" -L | grep -c kage-qa-tls-root) CA entry"
done <<< "${rows}"
