#!/bin/bash
# install.sh — build and deploy pressured as a root system service.
# Run: sudo ./install.sh   (from the repo root)
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$REPO/target/release/pressured"

if [[ $EUID -ne 0 ]]; then
    echo "error: run as root:  sudo ./install.sh"
    exit 1
fi

if [[ ! -x "$BIN" ]]; then
    echo "error: $BIN not found — build first with:  cargo build --release"
    exit 1
fi

# Notification runtime deps. Warn, never fail: a daemon that protects the desktop
# is still worth running when it can only report to the journal, and refusing to
# install over a missing notifier would be the tail wagging the dog.
#
# gdbus (not notify-send) because Ubuntu ships an AppArmor profile for notify-send
# that permits /run/user/*/bus — and inside the mount namespace this unit runs in,
# AppArmor cannot resolve that path back to the root namespace, hands the matcher a
# *relative* name, and denies the connect. gdbus and dbus-monitor carry no profile,
# so nothing mediates them. See src/notify.rs for the kernel's own account of it.
for tool in gdbus:libglib2.0-bin dbus-monitor:dbus-bin; do
    cmd="${tool%%:*}"; pkg="${tool##*:}"
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "warning: $cmd not found — desktop notifications will not render."
        echo "         Interventions still land in the journal (journalctl -u rtux.service)."
        echo "         Install with:  sudo apt install $pkg"
    fi
done

echo "Installing binary → /usr/local/bin/pressured"
install -m 0755 "$BIN" /usr/local/bin/pressured

HUD="$REPO/target/release/pressured-hud"
if [[ -x "$HUD" ]]; then
    echo "Installing HUD    → /usr/local/bin/pressured-hud"
    install -m 0755 "$HUD" /usr/local/bin/pressured-hud
    # Desktop file so `gapplication launch dev.pressured.Hud` can activate (and
    # focus) the window with a proper Wayland activation token.
    echo "Installing .desktop → /usr/share/applications/dev.pressured.Hud.desktop"
    install -m 0644 "$REPO/dev.pressured.Hud.desktop" /usr/share/applications/dev.pressured.Hud.desktop
else
    echo "(pressured-hud not built — run: cargo build --release --features hud)"
fi

# --- The socket group ---
# Everything that tells the daemon a human is present goes through
# /run/pressured.sock: the GNOME extension's focus reports and each session's
# `pressured ctl touch`. Both are unprivileged, so the socket needs a group they
# are in. A dedicated group rather than the desktop user's primary group,
# because a box can have two people logged in at once and a primary group can
# only serve one of them.
#
# Both clients ignore write failures on purpose (a shell extension must not spam
# the log; a UserPromptSubmit hook must not break a session), so if this group is
# missing or empty the result is a silent, total outage of rtux's model of human
# attention — with the daemon still reporting healthy. It happened on the dev box and
# ran for 26 hours. Hence the loudness below.
echo "Ensuring group    → pressured"
if ! getent group pressured >/dev/null; then
    groupadd -r pressured
    echo "  created system group 'pressured'"
fi
# Every human user (uid >= 1000 with a real home) — not just $SUDO_USER, since
# the person running the installer is not necessarily the only one who logs in.
added=""
while IFS=: read -r name _ uid _ _ home _; do
    [[ "$uid" -ge 1000 && "$uid" -lt 65534 && -d "$home" ]] || continue
    if ! id -nG "$name" | tr ' ' '\n' | grep -qx pressured; then
        usermod -aG pressured "$name" && added="$added $name"
    fi
done < <(getent passwd)
if [[ -n "$added" ]]; then
    echo "  added to group:$added"
    echo "  NOTE: group membership is read at LOGIN. Those users must log out and"
    echo "        back in before their focus reports and touches will be accepted."
else
    echo "  all desktop users already members"
fi

echo "Installing socket → /etc/systemd/system/pressured.socket"
install -m 0644 "$REPO/pressured.socket" /etc/systemd/system/pressured.socket

echo "Installing unit   → /etc/systemd/system/rtux.service"
install -m 0644 "$REPO/rtux.service" /etc/systemd/system/rtux.service

# Reconcile with systemd-oomd. Ubuntu's stock policy kills the largest cgroup in
# the user slice at 50% PSI pressure — the same band pressured works in — so the
# two race and oomd wins (it can SIGKILL the compositor's cgroup mid-mitigation,
# tearing down the session). Raise oomd's threshold to 80% so pressured acts
# first, keeping oomd as a hard backstop. Reversible: uninstall.sh removes it.
# Harmless on systems without systemd-oomd.
echo "Installing oomd drop-in → /etc/systemd/system/user@.service.d/50-pressured-oomd.conf"
install -d -m 0755 /etc/systemd/system/user@.service.d
install -m 0644 "$REPO/50-pressured-oomd.conf" /etc/systemd/system/user@.service.d/50-pressured-oomd.conf

echo "Reloading systemd and (re)starting service..."
systemctl daemon-reload
# Stop the service before the socket: with Requires=pressured.socket, restarting
# the socket under a live service leaves the daemon holding the old fd.
systemctl stop rtux.service 2>/dev/null || true
systemctl enable --now pressured.socket
systemctl enable rtux.service
systemctl restart rtux.service

echo
echo "--- systemctl status rtux.service ---"
systemctl --no-pager --full status rtux.service | head -20 || true

# The one check that would have caught the 26-hour outage: does the socket
# actually have the group we just set up? Cheap, and it fails loudly.
echo
echo "--- control socket ---"
if [[ -S /run/pressured.sock ]]; then
    sock_group=$(stat -c '%G' /run/pressured.sock)
    sock_mode=$(stat -c '%a' /run/pressured.sock)
    echo "  /run/pressured.sock  group=$sock_group  mode=$sock_mode"
    if [[ "$sock_group" != "pressured" ]]; then
        echo "  !! WRONG GROUP — expected 'pressured'. Focus reports and session"
        echo "     touches will be refused, and BOTH clients fail silently."
    else
        echo "  OK — the desktop users' group owns it."
        echo "  Verify the round trip as a real user (not root):"
        echo "     bash scripts/install-verify-focus.sh"
    fi
else
    echo "  !! /run/pressured.sock does not exist — check: systemctl status pressured.socket"
fi

# --- Offer zram, rtux's base-layer companion ---
# rtux keeps you responsive AT the memory wall; zram raises the wall so you
# rarely reach it. Offered (not forced) — it's a system-wide, but reversible,
# change. Control non-interactively with RTUX_ENABLE_ZRAM=1 (yes) / 0 (no).
zram_active() { swapon --show=NAME --noheadings 2>/dev/null | grep -q '^/dev/zram'; }
echo
if [[ "$(cat /sys/module/zswap/parameters/enabled 2>/dev/null)" == Y ]]; then
    # zram in front of a disk swapfile inverts the LRU (cold pages fill zram,
    # hot churn goes to disk); zswap does the same job without it.
    echo "zram: skipped — zswap is on (scripts/host-zram-to-zswap.sh)."
elif zram_active; then
    echo "zram: already active — nothing to do."
else
    case "${RTUX_ENABLE_ZRAM:-ask}" in
        1|yes|true)  do_zram=y ;;
        0|no|false)  do_zram=n ; echo "zram: skipped (RTUX_ENABLE_ZRAM=0)." ;;
        *)
            if [[ -t 0 ]]; then
                echo "This box has no compressed RAM swap (zram). rtux works best with it:"
                echo "it moves swap off the slow disk into RAM, so trivial load stops"
                echo "chugging. Recommended and reversible."
                read -r -p "Set up zram now? [Y/n] " ans
                [[ "$ans" =~ ^[Nn] ]] && do_zram=n || do_zram=y
            else
                do_zram=n
                echo "zram: non-interactive run — skipped. Add later: sudo ./setup-zram.sh"
                echo "      (or re-run this installer with RTUX_ENABLE_ZRAM=1)."
            fi ;;
    esac
    if [[ "${do_zram:-n}" == y ]]; then
        echo
        # Non-fatal: the core install already succeeded above, so a zram failure
        # (non-apt distro, apt lock, offline) must not make the whole run look
        # like a failure.
        if ! "$REPO/setup-zram.sh"; then
            echo "  zram setup did not complete — the core install is fine."
            echo "  Add zram later with:  sudo ./setup-zram.sh"
        fi
    elif [[ "${RTUX_ENABLE_ZRAM:-ask}" == "ask" && -t 0 ]]; then
        echo "Skipped zram. Enable later with:  sudo ./setup-zram.sh"
    fi
fi

echo
echo "Done. Follow it live with:  journalctl -u rtux.service -f"
