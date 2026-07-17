#!/usr/bin/env bash
# io-delegation-spike.sh — prove the io controller can be delegated to user apps,
# reversibly, then put the machine back exactly as it was.
#
# THE GAP (measured 2026-07-16 on rukh): user apps get only `cpu memory pids`
# delegated. `io` is available at the root and enabled for system.slice, but
# user.slice does NOT pass it down its subtree_control, so nothing under
# app.slice can set io.latency / io.max / io.weight. This script enables it via
# the systemd-sanctioned path (IOAccounting=yes), confirms `io` reaches
# app.slice, then reverts. Everything uses --runtime, so a reboot also clears it.
#
# Needs: sudo (system slices) + a reachable --user bus (run it from a terminal
# INSIDE your graphical session, not over a bare ssh/tty without XDG_RUNTIME_DIR).
#
# Run:  ./scripts/io-delegation-spike.sh
set -uo pipefail

CG=/sys/fs/cgroup
APP="$CG/user.slice/user-1000.slice/user@1000.service/app.slice"
SYS_UNITS=(user.slice user-1000.slice user@1000.service)

controllers() { cat "$APP/cgroup.controllers" 2>/dev/null; }
has_io()      { controllers | grep -qw io; }

revert() {
  echo
  echo "== reverting (restoring the exact prior state) =="
  systemctl --user set-property --runtime app.slice IOAccounting=no 2>/dev/null
  # set-property takes ONE unit per call, so revert each on its own.
  for u in "${SYS_UNITS[@]}"; do
    sudo systemctl set-property --runtime "$u" IOAccounting=no 2>/dev/null
  done
  # set-property --runtime writes drop-ins under /run; revert clears runtime state.
  sudo systemctl revert "${SYS_UNITS[@]}" >/dev/null 2>&1
  systemctl --user revert app.slice >/dev/null 2>&1
  sleep 1
  if has_io; then
    echo "!! WARNING: io still present on app.slice after revert."
    echo "   Clear it by rebooting (all changes were --runtime and do not persist)."
  else
    echo "ok — app.slice back to: [$(controllers)]"
  fi
}
trap revert EXIT

echo "== BEFORE =="
echo "app.slice controllers: [$(controllers)]"
if has_io; then
  echo "io is ALREADY delegated here — nothing to prove. Exiting without changes."
  trap - EXIT
  exit 0
fi

echo
echo "== enabling io accounting on the user chain (system, needs sudo) =="
# set-property takes ONE unit per call — loop, don't batch.
for u in "${SYS_UNITS[@]}"; do
  sudo systemctl set-property --runtime "$u" IOAccounting=yes || {
    echo "!! system set-property failed on $u (sudo declined?). Reverting."; exit 1; }
done

echo "== enabling io accounting on app.slice (--user) =="
systemctl --user set-property --runtime app.slice IOAccounting=yes || {
  echo "!! --user set-property failed (no session bus?). Reverting."; exit 1; }

sleep 1
echo
echo "== AFTER =="
echo "app.slice controllers: [$(controllers)]"

if has_io; then
  echo
  echo "PASS — io is now delegated to app.slice across the system/user boundary."
  # Prove it is actually usable: pick a live app scope and read its io.stat.
  scope=$(find "$APP" -maxdepth 1 -name '*.scope' -type d 2>/dev/null | head -1)
  if [ -n "$scope" ]; then
    echo "proof — $(basename "$scope") exposes:"
    for f in io.stat io.pressure io.latency io.max io.weight; do
      [ -e "$scope/$f" ] && echo "    $f  present" || echo "    $f  (absent)"
    done
  fi
  echo
  echo "=> The plug works. rtux's installer can enable it the same way, persistently"
  echo "   (drop the --runtime), gated on a capability check."
else
  echo
  echo "FAIL — io did not reach app.slice. The user-instance delegation did not take;"
  echo "the controller architecture should treat IO as capability-detected, not assumed."
fi
# trap runs revert on the way out.
