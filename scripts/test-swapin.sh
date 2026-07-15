#!/usr/bin/env bash
# test-swapin.sh — prove (or disprove) that touching a process's pages from
# outside faults its swapped-out anon memory back into RAM.
#
# Why this matters: memory.swap.max=0 only forbids FUTURE eviction. It does not
# recall pages already on disk, so a spine service that was swapped out before
# rtux protected it stays on disk — and the user keeps paying a major fault per
# keystroke. If this works, protection becomes curative instead of prophylactic.
#
# Needs root: reading another process's /proc/<pid>/mem requires
# PTRACE_MODE_ATTACH, and Yama ptrace_scope=1 denies it even to the same uid.
# CAP_SYS_PTRACE (which the rtux unit already holds) bypasses Yama.
#
# Run:  sudo ./scripts/test-swapin.sh

set -uo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "needs root (Yama ptrace_scope=$(cat /proc/sys/kernel/yama/ptrace_scope 2>/dev/null))" >&2
    echo "run: sudo $0" >&2
    exit 1
fi

BASE=/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/session.slice

# Touch one byte per page across every readable mapping of a pid.
toucher=$(mktemp /tmp/swapin-XXXXXX.py)
trap 'rm -f "$toucher"' EXIT
cat > "$toucher" <<'PY'
import sys, os
pid = sys.argv[1]
pg = os.sysconf("SC_PAGE_SIZE")
regions = []
with open(f"/proc/{pid}/maps") as f:
    for line in f:
        p = line.split()
        perms = p[1]
        path = p[5] if len(p) > 5 else ""
        if perms[0] != "r" or path.startswith("[v"):
            continue
        lo, hi = (int(x, 16) for x in p[0].split("-"))
        regions.append((lo, hi))
hit = err = 0
with open(f"/proc/{pid}/mem", "rb", buffering=0) as mem:
    for lo, hi in regions:
        for a in range(lo, hi, pg):
            try:
                mem.seek(a); mem.read(1); hit += 1
            except (OSError, ValueError):
                err += 1
print(f"    touched {hit} pages ({hit*pg/1048576:.1f}M), {err} unreadable")
PY

mb() { echo "scale=1; $1/1048576" | bc -l; }

report() {
    local cg=$1
    printf "    resident %sM  swapped %sM\n" \
        "$(mb "$(cat "$cg/memory.current")")" "$(mb "$(cat "$cg/memory.swap.current")")"
}

for unit in org.freedesktop.IBus.session.GNOME.service \
            org.gnome.SettingsDaemon.XSettings.service \
            wireplumber.service; do
    cg="$BASE/$unit"
    [ -r "$cg/memory.current" ] || { echo "$unit: no cgroup, skipping"; continue; }

    echo "=== $unit"
    echo "  before:"; report "$cg"

    for pid in $(cat "$cg/cgroup.procs"); do
        python3 "$toucher" "$pid" 2>/dev/null || echo "    pid $pid: unreadable"
    done

    echo "  after:"; report "$cg"
    echo
done
