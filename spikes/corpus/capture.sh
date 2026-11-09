#!/usr/bin/env bash
# Capture vtebench benchmark output as raw terminal-side bytes (post-onlcr), one .bin per benchmark.
# usage: capture.sh <vtebench-checkout> <out-dir> [min-mib=32]
# Each file = (ESC c + setup output + benchmark output) repeated until >= min-mib, exactly what
# vtebench's runner writes per sample (src/bench.rs run_sample). Scripts run under a 200x60 PTY
# because they size themselves with `tput` on their tty.
set -euo pipefail
src="$(realpath "${1:?vtebench checkout}")"
out="$(realpath "${2:?out dir}")"
min=$(( ${3:-32} * 1024 * 1024 ))
command -v script >/dev/null || { echo "util-linux 'script' not found (Fedora: dnf install util-linux-script)" >&2; exit 1; }

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT

# Run one script under a 200x60 PTY; raw bytes go to $2. Take script's stdout, not its log:
# the log always carries "Script started/done" framing, -q only silences stdout.
run_pty() {
  ( cd "$(dirname "$1")" && TERM=xterm-256color script -q -E never -c "stty cols 200 rows 60; exec '$1'" /dev/null </dev/null >"$2" )
}

for dir in "$src"/benchmarks/*/; do
  name="$(basename "$dir")"
  [[ -x "$dir/benchmark" ]] || continue
  : >"$tmp/setup"
  [[ -x "$dir/setup" ]] && run_pty "$dir/setup" "$tmp/setup"
  run_pty "$dir/benchmark" "$tmp/bench"
  bsize=$(stat -c %s "$tmp/bench")
  (( bsize > 0 )) || { echo "$name: empty benchmark output" >&2; exit 1; }
  # vtebench: benchmark payload repeated to >= 1 MiB per sample (bench.rs:136), setup once (bench.rs:178).
  # Exact N copies without N forks: double until big enough, then cut (the stream is periodic).
  want=$(( (1048576 + bsize - 1) / bsize * bsize ))
  while (( $(stat -c %s "$tmp/bench") < want )); do cat "$tmp/bench" "$tmp/bench" >"$tmp/b2"; mv "$tmp/b2" "$tmp/bench"; done
  { printf '\033c'; cat "$tmp/setup"; head -c "$want" "$tmp/bench"; } >"$tmp/sample"
  size=$(stat -c %s "$tmp/sample")
  : >"$out/$name.bin"
  for (( n = 0; n < (min + size - 1) / size; n++ )); do cat "$tmp/sample"; done >>"$out/$name.bin"
  printf '%s\t%d sample bytes\t%d total\n' "$name" "$size" "$(stat -c %s "$out/$name.bin")"
done
