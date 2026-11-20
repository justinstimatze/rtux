#!/bin/bash
# install-verify-focus.sh — verify that rtux's attention channel actually carries
# traffic, from the identity that really uses it.
#
#   bash scripts/install-verify-focus.sh          # check the invoking user
#   sudo bash scripts/install-verify-focus.sh      # check every logged-in user
#
# Why this script is different from its siblings. install-verify-cpu.sh,
# -killrung.sh, -naming.sh and -spine.sh all check that a thing EXISTS: a knob is
# set, a unit is active, a value landed. On 2026-08-07 every one of those passed
# on the dev box while the daemon had been deaf for 26 hours. The GNOME extension was
# installed, enabled, and byte-identical to the repo; the Claude touch hook was in
# settings.json exactly as install-touch-hook.sh writes it; the daemon was active
# and healthy. And /run/pressured.sock was root:root 0660, so every client got
# EACCES, and both clients ignore write failures by design — so nothing anywhere
# said a word. 38 of 38 focus events that day logged fg_pid=None. Zero focus
# thaws. rtux froze sessions a human was typing in.
#
# Presence checks cannot catch that. Only sending a real message from the real
# identity can. That is the whole idea here: do the round trip, as the user, and
# fail loudly.
set -uo pipefail

SOCK=/run/pressured.sock
fail=0
note() { printf '  %s\n' "$*"; }
bad()  { printf '  !! %s\n' "$*"; fail=1; }

echo "== daemon =="
if [[ "$(systemctl is-active rtux.service 2>/dev/null)" == active ]]; then
    note "rtux.service active   version: $(pressured --version 2>/dev/null || echo '?')"
else
    bad "rtux.service is not active — nothing below will mean anything."
    note "   systemctl status rtux.service"
    exit 1
fi

echo
echo "== socket =="
if [[ ! -S "$SOCK" ]]; then
    bad "$SOCK does not exist."
    note "   systemctl status pressured.socket"
    exit 1
fi
sock_group=$(stat -c '%G' "$SOCK")
sock_mode=$(stat -c '%a' "$SOCK")
note "$SOCK  owner=$(stat -c '%U:%G' "$SOCK")  mode=$sock_mode"
if [[ "$sock_group" == "root" ]]; then
    bad "group is root — this is the exact 2026-08-07 failure. No unprivileged"
    note "   client can connect, and none of them will tell you so."
    note "   Fix: sudo ./install.sh  (creates the 'pressured' group and the socket unit)"
elif [[ "$sock_group" != "pressured" ]]; then
    bad "group is '$sock_group', expected 'pressured'."
fi

# --- the round trip ---
# `ctl touch` is the right probe: it is exactly what the UserPromptSubmit hook
# runs, it addresses the caller's own cgroup (so it cannot affect anyone else),
# and it is idempotent — a touch that lands just refreshes a 300s TTL.
try_user() {
    local u="$1"
    local out rc
    if [[ "$(id -un)" == "$u" ]]; then
        out=$(pressured ctl touch 2>&1); rc=$?
    else
        out=$(runuser -u "$u" -- pressured ctl touch 2>&1); rc=$?
    fi
    if [[ $rc -eq 0 ]]; then
        printf '  OK    %-12s round trip succeeded: %s\n' "$u" "${out:-(no output)}"
        return 0
    fi
    printf '  !! %-12s EXIT %d: %s\n' "$u" "$rc" "${out//$'\n'/ }"
    if [[ "$out" == *"Permission denied"* ]]; then
        note "        In group 'pressured'? $(id -nG "$u" 2>/dev/null | tr ' ' '\n' | grep -qx pressured && echo yes || echo NO)"
        note "        If yes, they have not logged in since being added — group"
        note "        membership is read at login, so a re-login is required."
    fi
    fail=1
    return 1
}

echo
echo "== round trip (the part presence checks miss) =="
if [[ $EUID -eq 0 ]]; then
    # Every user with a live session, which is who the daemon actually hears from.
    users=$(loginctl list-sessions --no-legend 2>/dev/null | awk '{print $3}' | sort -u)
    [[ -n "$users" ]] || users="${SUDO_USER:-}"
    if [[ -z "$users" ]]; then
        bad "no logged-in users to test as. Re-run from a desktop session."
    fi
    for u in $users; do try_user "$u"; done
    note ""
    note '(as root, "pressured ctl touch" would succeed regardless of the group —'
    note ' which is why this script tests as the users instead. Testing as root is'
    note ' how a broken socket passes a verifier.)'
else
    try_user "$(id -un)"
fi

# --- the focus channel specifically ---
# The touch above proves the socket is reachable. It does not prove the GNOME
# extension is reporting, which is a separate client on the same socket.
echo
echo "== focus reports (GNOME extension → daemon) =="
UUID="rtux@justinstimatze.com"
target_home=$(getent passwd "${SUDO_USER:-$(id -un)}" | cut -d: -f6)
if [[ -d "$target_home/.local/share/gnome-shell/extensions/$UUID" ]]; then
    note "extension staged at ~/.local/share/gnome-shell/extensions/$UUID"
else
    bad "extension not installed — run: ./install-extension.sh"
fi

# Ask the journal whether any focus report has ever landed. `fg_pid=None` with no
# `thawed on focus:` anywhere is the signature of a dead channel; the diagnostic
# only prints during a freeze, so absence of BOTH is inconclusive on a quiet box.
since="-24h"
misses=$(journalctl -u rtux.service --since "$since" --no-pager -o cat 2>/dev/null \
         | grep -c 'fg_pid=None' || true)
hits=$(journalctl -u rtux.service --since "$since" --no-pager -o cat 2>/dev/null \
       | grep -c 'thawed on focus:' || true)
note "last 24h: fg_pid=None ×$misses, successful focus thaws ×$hits"
if [[ "$misses" -gt 0 && "$hits" -eq 0 ]]; then
    bad "the daemon has never resolved a foreground pid — the focus channel is dark."
    note "   If the round trip above passed, the socket is fine and the extension"
    note "   is the problem: check it is ENABLED (not merely staged) with"
    note "   'gnome-extensions list --enabled', and that the shell has been"
    note "   restarted since it was installed (log out and back in on Wayland)."
elif [[ "$misses" -eq 0 && "$hits" -eq 0 ]]; then
    note "inconclusive — nothing has been frozen recently, so neither line had"
    note "cause to print. Not a failure. Re-run after a pressure episode, or"
    note "provoke one with: bash scripts/pressure-test.sh"
fi

echo
if [[ $fail -eq 0 ]]; then
    echo "PASS — the attention channel carries traffic from a real user."
else
    echo "FAIL — see the !! lines above. rtux will freeze sessions you are using."
fi
exit $fail
