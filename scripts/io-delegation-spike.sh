#!/usr/bin/env bash
# io-delegation-spike.sh — delegate the io controller to user apps, then let you
# verify it after a re-login. Reversible: a /run drop-in that reboot-clears, or
# `--revert` to remove it now.
#
# THE GAP (traced 2026-07-16 on rukh, systemd 259): user apps get only
# `cpu memory pids` delegated. cgroup v2's io controller is available at the root
# and at user.slice, but user@1000.service runs with the Ubuntu vendor default
# `Delegate=pids memory cpu` (/usr/lib/systemd/system/user@.service:28), so the
# per-user `systemd --user` instance can never manage io on app.slice. The fix is
# a `Delegate=` drop-in adding `io cpuset` to that set.
#
# WHY THERE IS NO LIVE, NO-LOGOUT PROOF: a running user@N.service realises its
# delegated controllers once, at login, and will not re-delegate without a restart
# (which logs you out). The template unit exposes no resolved DelegateControllers to
# preview. So the ONLY ground truth is app.slice/cgroup.controllers after a fresh
# login. This script applies the change and hands you that one check.
#
# Needs: sudo (writes a /run drop-in + daemon-reload).
#
#   ./scripts/io-delegation-spike.sh            # apply, then re-login and check
#   ./scripts/io-delegation-spike.sh --revert   # remove the drop-in now
set -uo pipefail

DROPDIR=/run/systemd/system/user@.service.d
DROPIN="$DROPDIR/zz-pressured-io-spike.conf"
APP=/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice
CHECK="grep -qw io $APP/cgroup.controllers && echo 'PASS: io reached app.slice' || echo 'FAIL: still no io'"

if [ "${1:-}" = "--revert" ]; then
  sudo rm -f "$DROPIN"
  sudo rmdir "$DROPDIR" 2>/dev/null
  sudo systemctl daemon-reload
  echo "reverted — drop-in removed. Log out and back in to drop io from app.slice."
  exit 0
fi

echo "== current app.slice controllers =="
cat "$APP/cgroup.controllers"
if grep -qw io "$APP/cgroup.controllers"; then
  echo "io is ALREADY delegated to app.slice — nothing to do."
  exit 0
fi

echo
echo "== writing the Delegate= drop-in to /run (needs sudo) =="
sudo install -d -m 0755 "$DROPDIR" || { echo "!! could not create $DROPDIR"; exit 1; }
# A drop-in Delegate= replaces the value, so restate the vendor base set + io cpuset.
printf '[Service]\nDelegate=pids memory cpu io cpuset\n' \
  | sudo tee "$DROPIN" >/dev/null || { echo "!! could not write $DROPIN"; exit 1; }
sudo systemctl daemon-reload

cat <<EOF

Applied. This CANNOT take effect on your current session — user@1000.service
fixed its delegation at login. To activate and verify:

  1. Log out and back in (or reboot).
  2. Run this check:

     $CHECK

PASS means the installer can ship this as a persistent drop-in beside
50-pressured-oomd.conf and the daemon can drive io.latency on app scopes.

To undo without a reboot:  ./scripts/io-delegation-spike.sh --revert
(The drop-in lives under /run, so a reboot also clears it.)
EOF
