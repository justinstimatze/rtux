#!/bin/bash
# Install the kill-rung daemon and verify it's healthy AND quiet at idle — no
# spurious freezes or kills when pressure is normal. Run with:
#   sudo bash scripts/install-verify-killrung.sh
set -u
REPO=/home/gas6amus/Documents/rtux

echo "== installing $(${REPO}/target/release/pressured --version) =="
install -m755 "$REPO/target/release/pressured" /usr/local/bin/pressured
systemctl restart rtux.service
echo "restarted; observing 20s at idle..."
sleep 20

echo
echo "== daemon health =="
echo "  active: $(systemctl is-active rtux.service)   version: $(pressured --version)"

echo
echo "== SAFETY: no kills/freezes should have fired at normal pressure =="
bad=$(journalctl -u rtux.service --since "-25s" --no-pager -o cat 2>/dev/null \
      | grep -Ei 'killed |froze |failed to kill' || true)
if [ -z "$bad" ]; then
  echo "  OK — no kill/freeze activity at idle (correct)."
else
  echo "  !! UNEXPECTED activity at idle:"; echo "$bad" | sed 's/^/     /'
fi

echo
echo "== current memory pressure (context) =="
grep -E 'some|full' /proc/pressure/memory | sed 's/^/  /'
echo "  swap used: $(awk '/SwapTotal/{t=$2}/SwapFree/{f=$2}END{if(t)printf "%.0f%%\n",(t-f)/t*100; else print "n/a"}' /proc/meminfo)"

echo
echo "== spine still protected (from the prior commit) =="
B=/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/session.slice
pid=$(head -1 "$B/dbus.service/cgroup.procs" 2>/dev/null)
[ -n "$pid" ] && echo "  dbus.service oom_score_adj=$(cat /proc/$pid/oom_score_adj) (want -1000)"
