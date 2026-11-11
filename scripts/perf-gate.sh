#!/usr/bin/env bash
# Hot-path performance gate: v2's median must be <= 50% of v1's for
# `exec 1 -- true` and `ls`, both against the 17-chain migration fixture.
#
#   scripts/perf-gate.sh V1_BIN V2_BIN [MAX_RATIO]
#
# Every run uses scratch HOME/XDG directories and CHAINZ_DISABLE_KEYRING, so
# the caller's real config, keyring and state are never read or written.
set -euo pipefail

v1=$(realpath "$1")
v2=$(realpath "$2")
max_ratio=${3:-0.50}
here=$(cd "$(dirname "$0")/.." && pwd)
fixture="$here/crates/chainz-core/tests/fixtures/v1-config.json"

work=$(mktemp -d "${TMPDIR:-/tmp}/chainz-perf.XXXXXX")
trap 'rm -rf "$work"' EXIT

# One sandbox serves both: v1 reads config.json, v2 reads config.toml (and
# ignores a leftover config.json once config.toml exists).
root="$work/sandbox"
mkdir -p "$root"/{home,config/chainz,state,cache,runtime}
chmod 700 "$root/config/chainz"
export HOME="$root/home" XDG_CONFIG_HOME="$root/config" XDG_STATE_HOME="$root/state" \
  XDG_CACHE_HOME="$root/cache" XDG_RUNTIME_DIR="$root/runtime" CHAINZ_DISABLE_KEYRING=1
unset CHAINZ_CHAIN ETH_RPC_URL CHAIN_ID
cp "$fixture" "$root/config/chainz/config.json"
"$v2" config migrate >/dev/null 2>&1
test -f "$root/config/chainz/config.toml"
cp "$fixture" "$root/config/chainz/config.json"

# Sanity: both resolve chain 1 and run the command.
"$v1" exec 1 -- true
"$v2" exec 1 -- true

bench() { # label v1-args v2-args
  local label=$1 a1=$2 a2=$3
  hyperfine -N --warmup 10 --runs "${RUNS:-200}" --style basic \
    --export-json "$work/$label.json" \
    -n v1 "$v1 $a1" -n v2 "$v2 $a2" >/dev/null
  local m1 m2
  m1=$(jq '.results[] | select(.command=="v1") | .median' "$work/$label.json")
  m2=$(jq '.results[] | select(.command=="v2") | .median' "$work/$label.json")
  awk -v l="$label" -v a="$m1" -v b="$m2" -v max="$max_ratio" 'BEGIN {
    r = b / a
    printf "%-6s v1 %.2f ms  v2 %.2f ms  ratio %.2f (max %.2f)\n", l, a*1000, b*1000, r, max
    exit (r <= max) ? 0 : 1
  }'
}

status=0
bench exec "exec 1 -- true" "exec 1 -- true" || status=1
bench ls "list" "ls" || status=1
exit $status
