#!/usr/bin/env bash
# Print one version's section of CHANGELOG.md (without its heading), the
# GitHub release notes. Fails when the section is missing or empty.
#
#   scripts/release-notes.sh 2.0.0
set -euo pipefail
cd "$(dirname "$0")/.."

version=${1:?usage: scripts/release-notes.sh VERSION}
notes=$(awk -v head="## $version " '
  index($0, head) == 1 { on = 1; next }
  on && /^## / { exit }
  on { print }
' CHANGELOG.md)

if [ -z "${notes//[[:space:]]/}" ]; then
  echo "CHANGELOG.md has no section for $version" >&2
  exit 1
fi
printf '%s\n' "$notes"
