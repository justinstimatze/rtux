#!/bin/bash
# bench.sh — does freezing the memory hog actually keep a foreground app
# responsive, or is it theatre? Runs a "victim" (a stand-in foreground app that
# sweeps a working set each tick) against a runaway "hog", twice:
#
#   A) unmanaged  — the hog balloons; the victim thrashes to swap.
#   B) freeze     — partway through, we freeze the hog (cgroup.freeze — the exact
#                   primitive rtux's actions::freeze_cgroup writes), then measure.
#
# SAFETY: the whole thing runs inside a memory-capped `bench.slice`, so the
# pressure is contained — your real apps (browser, editors) are never touched and
# the machine as a whole is never driven into swap. Worst case the benchmark's
# own processes are OOM-killed inside their sandbox.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SLICE=bench.slice
BASE=/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/$SLICE
export VICTIM_WS_MB=${VICTIM_WS_MB:-250}
export VICTIM_DURATION_S=${VICTIM_DURATION_S:-24}
export HOG_STEP_MB=${HOG_STEP_MB:-50}
export HOG_MAX_MB=${HOG_MAX_MB:-2000}
FREEZE_AT_S=${FREEZE_AT_S:-9}   # into run B, when to freeze the hog

cleanup() {
  # thaw first (a frozen unit won't stop), then stop everything.
  [ -w "$BASE/bench-hog.service/cgroup.freeze" ] && echo 0 > "$BASE/bench-hog.service/cgroup.freeze" 2>/dev/null
  systemctl --user stop bench-hog.service bench-victim.service 2>/dev/null
  systemctl --user reset-failed 2>/dev/null
}
trap cleanup EXIT

run() {
  local mode="$1"
  cleanup; sleep 0.5
  # steady runaway hog, contained in the capped slice
  systemd-run --user --slice=$SLICE --unit=bench-hog \
    --setenv=HOG_STEP_MB=$HOG_STEP_MB --setenv=HOG_MAX_MB=$HOG_MAX_MB \
    python3 "$HERE/hog.py" >/dev/null 2>&1

  if [ "$mode" = freeze ]; then
    ( sleep "$FREEZE_AT_S"
      # the rtux primitive: pause the whole hog cgroup at once
      [ -w "$BASE/bench-hog.service/cgroup.freeze" ] \
        && echo 1 > "$BASE/bench-hog.service/cgroup.freeze" 2>/dev/null \
        || systemctl --user kill -s SIGSTOP bench-hog.service 2>/dev/null
    ) &
  fi

  # victim runs in the same capped slice; capture its JSON summary
  systemd-run --user --slice=$SLICE --unit=bench-victim -P --wait \
    --setenv=VICTIM_WS_MB=$VICTIM_WS_MB --setenv=VICTIM_DURATION_S=$VICTIM_DURATION_S \
    python3 "$HERE/victim.py" 2>/dev/null
  wait 2>/dev/null
}

echo "Configuring capped $SLICE (contained — your real apps are untouched)…"
systemctl --user set-property $SLICE MemoryMax=1200M MemorySwapMax=1500M 2>/dev/null

echo "=== A) unmanaged (hog runs free) ==="
A=$(run unmanaged); echo "$A"
echo "=== B) freeze the hog at ${FREEZE_AT_S}s ==="
B=$(run freeze); echo "$B"

echo
echo "=== victim responsiveness: A (unmanaged) vs B (freeze) ==="
python3 - "$A" "$B" <<'PY'
import json, sys
a, b = json.loads(sys.argv[1]), json.loads(sys.argv[2])
def row(k, label, unit="ms", better="lower"):
    av, bv = a.get(k,0), b.get(k,0)
    delta = ""
    if av and bv is not None:
        if better=="lower" and bv>0:
            delta = f"  ({av/bv:.1f}x better)" if bv<av else ""
    print(f"  {label:22} A={av:>8}{unit}   B={bv:>8}{unit}{delta}")
row("p50_ms", "median sweep")
row("p95_ms", "p95 sweep")
row("p99_ms", "p99 sweep")
row("max_ms", "worst sweep")
row("janky_over_50ms",  "janky sweeps >50ms",  unit="")
row("janky_over_200ms", "janky sweeps >200ms", unit="")
print(f"  completed sweeps       A={a.get('sweeps')}        B={b.get('sweeps')}   (more = less stalled)")
PY
