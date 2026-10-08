#!/usr/bin/env bash
# Checks commit subjects against Conventional Commits 1.0.
#
#   check-conventional-commits.sh --subject "feat(m0): repository scaffold"
#   check-conventional-commits.sh --range origin/main..HEAD
#   check-conventional-commits.sh --file .git/COMMIT_EDITMSG     (commit-msg hook)
set -euo pipefail

types='build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test'
pattern="^(${types})(\([a-z0-9][a-z0-9._/-]*\))?!?: [^[:space:]].*$"

usage() {
    echo "usage: $0 --subject <text> | --range <rev-range> | --file <commit-message-file>" >&2
    exit 2
}

# Prints a diagnostic and returns 1 when the subject is not conventional.
check_subject() {
    local subject="$1" origin="$2"
    if [[ "${subject}" =~ ${pattern} ]]; then
        return 0
    fi
    echo "error: not a conventional commit subject (${origin}):" >&2
    echo "    ${subject}" >&2
    echo "  expected: <type>(<scope>)?!?: <description>" >&2
    echo "  types:    ${types//|/, }" >&2
    return 1
}

[ "$#" -eq 2 ] || usage
mode="$1"
value="$2"
status=0

case "${mode}" in
    --subject)
        check_subject "${value}" "subject" || status=1
        ;;
    --range)
        # Merge commits carry git's own subject and are not authored text.
        while IFS=$'\t' read -r sha subject; do
            [ -n "${sha}" ] || continue
            check_subject "${subject}" "commit ${sha}" || status=1
        done < <(git log --no-merges --format='%h%x09%s' "${value}")
        ;;
    --file)
        subject="$(head -n 1 "${value}")"
        # Autosquash commits are folded away before a branch is merged.
        case "${subject}" in
            'fixup! '* | 'squash! '* | 'amend! '*) ;;
            *) check_subject "${subject}" "commit message" || status=1 ;;
        esac
        ;;
    *)
        usage
        ;;
esac

exit "${status}"
