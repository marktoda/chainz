#!/usr/bin/env bash
# The checks CI runs, in one place: formatting, lints, rustdoc and tests.
#
#   scripts/check.sh            # everything
#   scripts/check.sh lint       # fmt, clippy and rustdoc only
#   scripts/check.sh test       # tests only
#
# Set CHAINZ_REQUIRE_TOOLS=1 to fail, rather than skip, the tests that need
# anvil or strace. Extra arguments after the mode go to `cargo test`
# (for example `scripts/check.sh test -- --test-threads=2`).
set -euo pipefail
cd "$(dirname "$0")/.."

mode=${1:-all}
[ $# -gt 0 ] && shift

lint() {
  cargo fmt --all -- --check
  cargo clippy --locked --workspace --all-targets -- -D warnings
  # chainz-tui's dev-dependency turns on chainz-serve/testing for the whole
  # workspace; lint the gateway as it ships, without it.
  cargo clippy --locked -p chainz-serve --lib -- -D warnings
  RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps
}

tests() {
  cargo test --locked --workspace --all-targets "$@"
}

case $mode in
  all) lint && tests "$@" ;;
  lint) lint ;;
  test) tests "$@" ;;
  *) echo "usage: scripts/check.sh [all|lint|test] [-- cargo test args]" >&2; exit 2 ;;
esac
