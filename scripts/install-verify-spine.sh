#!/bin/bash
# Install the freshly-built daemon, restart it, and verify the session-spine
# OOM protection landed. Run with: sudo bash scripts/install-verify-spine.sh
set -u
REPO=/home/gas6amus/Documents/rtux
B=/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/session.slice

echo "== installing $(${REPO}/target/release/pressured --version) =="
install -m755 "$REPO/target/release/pressured" /usr/local/bin/pressured
systemctl restart rtux.service
echo "restarted; waiting 35s for the first protection retry to land..."
sleep 35

echo
echo "== protection log (want 'protected …' lines, NOT endless 'not protected yet') =="
journalctl -u rtux.service --since "-1min" --no-pager -o cat \
  | grep -Ei 'protecting critical|protected |not protected' || echo "(none)"

echo
echo "== spine oom_score_adj (want -1000) + memory.min =="
for u in dbus.service org.gnome.Shell@ubuntu.service pipewire-pulse.service; do
  pid=$(head -1 "$B/$u/cgroup.procs" 2>/dev/null)
  [ -n "$pid" ] && printf '  %-34s oom_score_adj=%s  memory.min=%s\n' \
    "$u" "$(cat /proc/$pid/oom_score_adj 2>/dev/null)" "$(cat $B/$u/memory.min 2>/dev/null)"
done

echo
echo "== leak check: a hog must be UNTOUCHED by rtux (not forced to -1000 by us) =="
# NB: Claude sessions self-set -1000 already; the point is rtux didn't touch a
# non-spine service. Check a settings-daemon (was +200, should STILL be +200).
sd="$B/org.gnome.SettingsDaemon.Housekeeping.service"
pid=$(head -1 "$sd/cgroup.procs" 2>/dev/null)
[ -n "$pid" ] && echo "  SettingsDaemon.Housekeeping oom_score_adj=$(cat /proc/$pid/oom_score_adj) (want unchanged, e.g. 200)"
