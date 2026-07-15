#!/usr/bin/env bash
# verify-realign.sh — check that the standing-guarantee realignment actually took
# effect on the live system. Run after `sudo ./install.sh`.
#
# Checks, in order of how load-bearing they are:
#   1. the spine is pinned  (memory.min > 0, oom_score_adj = -1000, swap.max = 0)
#   2. the bulk ceiling is set  (app.slice/memory.high != max)
#   3. fault-in is recalling swap  (session.slice swap trending toward 0)
#
# Needs no root: everything here is world-readable except oom_score_adj of
# other-uid pids, and the spine runs as the desktop user.

set -uo pipefail

U=/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service
S=$U/session.slice
A=$U/app.slice

mb() { echo "scale=1; ${1:-0}/1048576" | bc -l; }
ok()   { printf "  \033[32m✓\033[0m %s\n" "$1"; }
bad()  { printf "  \033[31m✗\033[0m %s\n" "$1"; }

echo "=== 1. spine pinned? ==="
for u in org.gnome.Shell@ubuntu.service \
         org.freedesktop.IBus.session.GNOME.service \
         wireplumber.service pipewire.service dbus.service \
         org.gnome.SettingsDaemon.XSettings.service \
         org.gnome.SettingsDaemon.Keyboard.service \
         xdg-desktop-portal.service xdg-desktop-portal-gnome.service \
         dconf.service; do
    cg="$S/$u"
    [ -r "$cg/memory.current" ] || continue
    min=$(cat "$cg/memory.min" 2>/dev/null || echo 0)
    swapmax=$(cat "$cg/memory.swap.max" 2>/dev/null || echo max)
    cur=$(cat "$cg/memory.current"); sw=$(cat "$cg/memory.swap.current")
    pid=$(head -1 "$cg/cgroup.procs" 2>/dev/null)
    adj=$(cat "/proc/$pid/oom_score_adj" 2>/dev/null || echo "?")

    label=$(printf "%-42s res=%7sM swap=%7sM min=%6sM swap.max=%-4s oom=%s" \
        "${u%.service}" "$(mb "$cur")" "$(mb "$sw")" "$(mb "$min")" "$swapmax" "$adj")
    if [ "$min" -gt 0 ] 2>/dev/null && [ "$adj" = "-1000" ]; then
        ok "$label"
    else
        bad "$label"
    fi
done

echo
echo "=== 2. bulk ceiling set? ==="
high=$(cat "$A/memory.high" 2>/dev/null || echo max)
if [ "$high" = "max" ]; then
    bad "app.slice memory.high = max  (ceiling NOT applied)"
else
    ok "app.slice memory.high = $(mb "$high")M  (current $(mb "$(cat $A/memory.current)")M)"
fi

echo
echo "=== 3. fault-in recalling swap? ==="
echo "  session.slice swap now: $(mb "$(cat $S/memory.swap.current)")M"
echo "  (re-run in a few minutes — gnome-shell's ~490M heals at 64M per 30s pass)"
echo
echo "=== journal (what the daemon said at startup) ==="
journalctl -u rtux.service --no-pager -n 25 -o cat 2>/dev/null \
    || echo "  (need: sudo journalctl -u rtux.service -n 25)"
