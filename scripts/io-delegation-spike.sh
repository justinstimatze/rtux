#!/usr/bin/env bash
# io-delegation-spike.sh — prove the io controller can be delegated to user apps,
# reversibly, then put the machine back exactly as it was.
#
# THE GAP (traced 2026-07-16 on rukh, systemd 259): user apps get only
# `cpu memory pids` delegated. cgroup v2's io controller is available at the root
# and at user.slice, but user@1000.service runs with the Ubuntu vendor default
# `Delegate=pids memory cpu` (/usr/lib/systemd/system/user@.service:28), so the
# per-user `systemd --user` instance is structurally forbidden from managing io on
# anything it owns — app.slice included. IOAccounting=yes is the WRONG lever: you
# cannot account for a controller you were never delegated. The right lever is
# Delegate=: add `io cpuset` to the delegated set on user@.service.
#
# The realised cgroup subtree of a running user@N.service is fixed at login, so the
# change only reaches a live app.slice for sessions started AFTER a daemon-reload —
# re-delegating a running session would mean restarting user@1000.service, which
# tears the whole desktop down. So this spike proves the SYSTEMD SIDE without a
# logout: it writes a /run drop-in (reboot-clears), reloads, and confirms
# user@.service now reports `io cpuset` in its delegated set — i.e. systemd accepts
# the config. The controller actually landing in app.slice is a next-login step.
#
# Needs: sudo (writes a /run drop-in + daemon-reload). No --user bus required.
#
# Run:  ./scripts/io-delegation-spike.sh
set -uo pipefail

UNIT=user@.service
DROPDIR=/run/systemd/system/user@.service.d
DROPIN="$DROPDIR/zz-pressured-io-spike.conf"

delegated() { systemctl show "$UNIT" -p DelegateControllers --value 2>/dev/null; }
has_io()    { delegated | grep -qw io; }

revert() {
  echo
  echo "== reverting (restoring the exact prior state) =="
  sudo rm -f "$DROPIN"
  sudo rmdir "$DROPDIR" 2>/dev/null   # only succeeds if we created it and it's empty
  sudo systemctl daemon-reload
  if has_io; then
    echo "!! WARNING: io still in the delegated set after revert."
    echo "   Clear it by rebooting (the drop-in was under /run and does not persist)."
  else
    echo "ok — user@.service back to delegating: [$(delegated)]"
  fi
}
trap revert EXIT

echo "== BEFORE =="
echo "user@.service delegates: [$(delegated)]"
if has_io; then
  echo "io is ALREADY delegated — nothing to prove. Exiting without changes."
  trap - EXIT
  exit 0
fi

echo
echo "== writing a runtime Delegate= drop-in (needs sudo) =="
sudo install -d -m 0755 "$DROPDIR" || { echo "!! could not create $DROPDIR"; exit 1; }
# Vendor default is `Delegate=pids memory cpu`; add io + cpuset (cpuset for the
# CPU-idle effector). A drop-in Delegate= replaces the value, so restate the base set.
printf '[Service]\nDelegate=pids memory cpu io cpuset\n' \
  | sudo tee "$DROPIN" >/dev/null || { echo "!! could not write $DROPIN"; exit 1; }

echo "== daemon-reload so systemd re-reads user@.service =="
sudo systemctl daemon-reload

echo
echo "== AFTER =="
echo "user@.service delegates: [$(delegated)]"

if has_io; then
  echo
  echo "PASS — systemd accepts io in the delegated set for user@.service."
  echo "   The controller reaches app.slice for sessions started after this reload;"
  echo "   your CURRENT session keeps its old delegation until next login."
  echo
  echo "=> The plug works. rtux's installer can ship this as a persistent drop-in"
  echo "   next to 50-pressured-oomd.conf (drop the /run path), gated on a"
  echo "   capability check, and prompt for a re-login to activate it live."
else
  echo
  echo "FAIL — io did not enter the delegated set. This build/policy refuses io"
  echo "delegation to user@.service; the controller should treat IO as"
  echo "capability-detected, not assumed."
fi
# trap runs revert on the way out.
