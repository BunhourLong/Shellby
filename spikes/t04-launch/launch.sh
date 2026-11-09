#!/usr/bin/env bash
# Same-clock launcher (T04 Design §3 method #1).
# Usage: launch.sh N [t04-gpu ARGS...]   (env T04_BIN overrides the binary path)
# t0 = CLOCK_MONOTONIC read right before spawn (python3 time.monotonic_ns is CLOCK_MONOTONIC on Linux;
# `date +%s%N` is CLOCK_REALTIME, so it is not used). The spike reads the same clock via clock_gettime
# at first present() return and prints launch_to_first_present_us. Includes fork/exec, dynamic linking, pre-main.
set -euo pipefail
[[ $# -ge 1 && $1 =~ ^[0-9]+$ && $1 -ge 1 ]] || { echo "usage: $0 N [t04-gpu args...]" >&2; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)
bin=${T04_BIN:-$here/../t04-gpu/target/release/t04-gpu}
[[ -x $bin ]] || { echo "missing $bin (cargo build --release in spikes/t04-gpu)" >&2; exit 2; }
exec python3 -I - "$bin" "$@" <<'PY'
import math, os, subprocess, sys, time
bin_, n, args = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
base = dict(os.environ, NT_EXIT_AFTER_FIRST_FRAME="1")
vals = []
for i in range(n):
    env = dict(base)
    t0 = time.monotonic_ns()
    env["NT_LAUNCH_T0_NS"] = str(t0)
    p = subprocess.run([bin_, *args], env=env, capture_output=True, text=True)
    line = [l for l in p.stdout.splitlines() if l.startswith("launch_to_first_present_us\t")]
    if p.returncode != 0 or not line:
        sys.exit(f"run {i}: exit {p.returncode}, no launch line\n{p.stderr[-2000:]}")
    vals.append(int(line[0].split("\t")[1]) / 1000)
v = sorted(vals)
pct = lambda q: v[max(1, math.ceil(q * len(v))) - 1]  # nearest rank
print(f"launch_to_first_present_ms n={n} args={' '.join(args) or '(default)'}")
print(f"p50 {pct(0.50):.2f}\tp90 {pct(0.90):.2f}\tmin {v[0]:.2f}\tmax {v[-1]:.2f}")
PY
