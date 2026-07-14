#!/usr/bin/env bash
# pressure-test.sh — deliberately generate memory pressure so you can watch
# rtux's mitigation ladder fire on demand, instead of waiting for an organic
# episode.
#
# The hog runs inside a transient *user* scope (under your app.slice), so rtux
# sees it as an ordinary freezable/killable app — just like a browser tab or a
# background build, not a terminal you're typing in. The hog:
#   * allocates resident memory in steps up to a ceiling,
#   * prints memory PSI + MemAvailable + SwapFree after each step,
#   * SELF-ABORTS if free memory drops under a hard floor, so this test can
#     never become the very out-of-memory crash it is meant to exercise safely,
#   * holds at peak (so you can watch rtux pause + reclaim-to-zram this scope),
#     then releases and exits — the scope tears down cleanly.
#
# What you should see rtux do (in another pane):
#   journalctl -u rtux.service -f -o cat | grep -iE 'Paused|Froze|thawed|killed|throttled|un-throttled'
#   watch -n1 pressured ctl history
# Expected on a healthy default run: Eased off / throttled (Elevated), then
# Paused + reclaimed (Critical), then Resumed once this scope releases. A *kill*
# should appear only if swap actually climbs past ~85% (see rtux's SWAP_HIGH_WATER).
#
# Usage: scripts/pressure-test.sh [TARGET_MB] [HOLD_SECS]
#   TARGET_MB   total to allocate (default 6144 = 6 GB)
#   HOLD_SECS   seconds to hold at peak before releasing (default 40)
# Ctrl+C tears the hog (and its scope) down immediately.
set -euo pipefail

TARGET_MB="${1:-6144}"
HOLD_SECS="${2:-40}"
STEP_MB=256
FLOOR_MB=400          # abort if MemAvailable drops below this — the safety rail

if ! command -v systemd-run >/dev/null 2>&1; then
  echo "systemd-run not found — cannot isolate the hog in its own scope." >&2
  exit 1
fi
if [ "$(id -u)" = "0" ]; then
  echo "Run as your normal user, not root: the hog must land in your user" >&2
  echo "app.slice so rtux treats it like an ordinary app (and spares the" >&2
  echo "terminal you're actually in)." >&2
  exit 1
fi

echo "rtux pressure test: allocating up to ${TARGET_MB} MB (step ${STEP_MB} MB), holding ${HOLD_SECS}s at peak."
echo "Safety floor: aborts allocation if MemAvailable < ${FLOOR_MB} MB. Ctrl+C tears the hog down."
echo "Scope: rtux-pressure-test.scope (under your app.slice)."
echo

exec systemd-run --user --scope --unit=rtux-pressure-test --quiet \
  python3 - "$TARGET_MB" "$STEP_MB" "$HOLD_SECS" "$FLOOR_MB" <<'PY'
import sys, time, mmap

target_mb, step_mb, hold_s, floor_mb = (int(x) for x in sys.argv[1:5])
step = step_mb * 1024 * 1024
chunk = b"\x00" * (4 * 1024 * 1024)  # touch pages in 4 MB writes (no 256 MB spike)

def meminfo():
    d = {}
    with open("/proc/meminfo") as f:
        for line in f:
            k, _, v = line.partition(":")
            d[k.strip()] = int(v.strip().split()[0]) // 1024  # -> MB
    return d

def psi10():
    try:
        with open("/proc/pressure/memory") as f:
            for tok in f.readline().split():           # "some avg10=.. .."
                if tok.startswith("avg10="):
                    return float(tok.split("=", 1)[1])
    except OSError:
        pass
    return 0.0

blocks = []
allocated = 0
print(f"{'ALLOC':>8} {'PSI10':>7} {'MemAvail':>10} {'SwapFree':>10}", flush=True)
try:
    while allocated < target_mb:
        mi = meminfo()
        if mi.get("MemAvailable", 0) < floor_mb:
            print(f"\n!! MemAvailable {mi['MemAvailable']} MB < floor {floor_mb} MB "
                  f"— aborting allocation (safety rail).", flush=True)
            break
        b = mmap.mmap(-1, step)
        for off in range(0, step, len(chunk)):
            b.write(chunk)                              # dirty every page -> resident
        blocks.append(b)
        allocated += step_mb
        mi = meminfo()
        print(f"{allocated:>6}MB {psi10():>7.1f} {mi.get('MemAvailable',0):>8}MB "
              f"{mi.get('SwapFree',0):>8}MB", flush=True)
        time.sleep(0.4)

    print(f"\nHolding {allocated} MB for {hold_s}s — watch rtux pause/reclaim this scope…", flush=True)
    end = time.monotonic() + hold_s
    while time.monotonic() < end:
        mi = meminfo()
        print(f"  hold: PSI10={psi10():.1f} MemAvail={mi.get('MemAvailable',0)}MB "
              f"SwapFree={mi.get('SwapFree',0)}MB", flush=True)
        time.sleep(3)
finally:
    print("Releasing. Scope will exit and tear down.", flush=True)
    blocks.clear()
PY
