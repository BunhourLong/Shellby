#!/usr/bin/env bash
# The single definition of green for newTerm. Gates run cheapest first;
# the exit code is the number of the first failing gate.
#   scripts/check.sh          all gates
#   scripts/check.sh --fast   gates 1-2 only (fmt + source greps; used by the pre-commit hook)
# Portable to macOS bash 3.2: no mapfile, no associative arrays.
set -euo pipefail
cd "$(dirname "$0")/.."

fast=0
case "${1:-}" in
  --fast) fast=1 ;;
  "") ;;
  *) echo "usage: scripts/check.sh [--fast]" >&2; exit 64 ;;
esac

fail() { echo "FAIL [$1]: $2" >&2; exit "$1"; }

# Spikes live outside crates/ and are not workspace members; every gate is scoped to crates/.
rs_files() { find crates -name '*.rs' -type f; }

echo "== 1 fmt"
cargo fmt --all --check || fail 1 "run: cargo fmt --all"

echo "== 2 source greps"
if rs_files | xargs grep -nE '\b(todo|unimplemented|dbg)!' ; then
  fail 2 "todo!/unimplemented!/dbg! left in crates/ (listed above)"
fi
# Every `unsafe {` block and `unsafe impl` needs `SAFETY:` on its own line or within the 3 lines above.
# Clippy's undocumented_unsafe_blocks misses `unsafe impl`; this covers both.
unsafe_hits=$(rs_files | xargs awk '
  FNR == 1 { p1 = p2 = p3 = "" }
  /unsafe[[:space:]]*\{|unsafe[[:space:]]+impl/ {
    if ((p1 $0 p2 p3) !~ /SAFETY:/) print FILENAME ":" FNR ": unsafe without SAFETY:"
  }
  { p3 = p2; p2 = p1; p1 = $0 }
')
if [ -n "$unsafe_hits" ]; then
  echo "$unsafe_hits" >&2
  fail 2 "unsafe without a // SAFETY: comment (listed above)"
fi
shortcuts=$(rs_files | xargs grep -n '// shortcut:' || true)
if [ -n "$shortcuts" ]; then
  echo "info: $(echo "$shortcuts" | wc -l | tr -d ' ') shortcut(s):"
  echo "$shortcuts"
fi

if [ "$fast" = 1 ]; then echo "ok (fast)"; exit 0; fi

echo "== 3 clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings || fail 3 "clippy"

echo "== 4 test"
cargo test --workspace --locked || fail 4 "tests"

echo "== 5 macOS check"
mac=aarch64-apple-darwin
if rustup target list --installed 2>/dev/null | grep -qx "$mac"; then
  # Type-checks cfg(target_os = "macos") code only: no linking, C build scripts may differ.
  cargo check --workspace --target "$mac" --locked || fail 5 "cargo check --target $mac"
else
  echo "skip: macOS target not installed (rustup target add $mac)"
fi

echo "ok"
