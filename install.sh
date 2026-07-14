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
systemctl enable rtux.service
systemctl restart rtux.service

echo
echo "--- systemctl status rtux.service ---"
systemctl --no-pager --full status rtux.service | head -20 || true

# --- Offer zram, rtux's base-layer companion ---
# rtux keeps you responsive AT the memory wall; zram raises the wall so you
# rarely reach it. Offered (not forced) — it's a system-wide, but reversible,
# change. Control non-interactively with RTUX_ENABLE_ZRAM=1 (yes) / 0 (no).
zram_active() { swapon --show=NAME --noheadings 2>/dev/null | grep -q '^/dev/zram'; }
echo
if zram_active; then
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
