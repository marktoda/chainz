#!/usr/bin/env bash
# Print one version's section of CHANGELOG.md (without its heading), the
# GitHub release notes. Fails when the section is missing or empty. With
# --release, also fails unless the heading carries a date
# (`## 2.0.0 - 2026-11-20`, not `- Unreleased`).
#
#   scripts/release-notes.sh 2.0.0              # preview
#   scripts/release-notes.sh --release 2.0.0    # the release job
set -euo pipefail
cd "$(dirname "$0")/.."

release=false
if [ "${1:-}" = "--release" ]; then
  release=true
  shift
fi
version=${1:?usage: scripts/release-notes.sh [--release] VERSION}
notes=$(awk -v head="## $version " '
  index($0, head) == 1 { on = 1; next }
  on && /^## / { exit }
  on { print }
' CHANGELOG.md)

if [ -z "${notes//[[:space:]]/}" ]; then
  echo "CHANGELOG.md has no section for $version" >&2
  exit 1
fi
if $release && ! grep -qE "^## ${version//./\\.} - [0-9]{4}-[0-9]{2}-[0-9]{2}\$" CHANGELOG.md; then
  echo "CHANGELOG.md's $version heading has no release date (still Unreleased?)" >&2
  exit 1
fi
printf '%s\n' "$notes"
