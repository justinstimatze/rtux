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

TRAY="$REPO/target/release/pressured-tray"
if [[ -x "$TRAY" ]]; then
    echo "Installing tray   → /usr/local/bin/pressured-tray"
    install -m 0755 "$TRAY" /usr/local/bin/pressured-tray
    # Autostart the indicator for the real (non-root) user on each login.
    if [[ -n "${SUDO_USER:-}" ]]; then
        UHOME=$(getent passwd "$SUDO_USER" | cut -d: -f6)
        AUTO="$UHOME/.config/autostart"
        mkdir -p "$AUTO"
        cat > "$AUTO/pressured-tray.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=rtux pressure indicator
Exec=/usr/local/bin/pressured-tray
X-GNOME-Autostart-enabled=true
NoDisplay=true
EOF
        # Use the user's real primary group (not always == username).
        UGROUP=$(id -gn "$SUDO_USER" 2>/dev/null || echo "$SUDO_USER")
        chown "$SUDO_USER:$UGROUP" "$AUTO/pressured-tray.desktop"
        echo "  autostart added for $SUDO_USER (starts on each login)"
    else
        echo "  (no SUDO_USER — tray autostart skipped; run install.sh via 'sudo',"
        echo "   or add ~/.config/autostart/pressured-tray.desktop yourself)"
    fi
else
    echo "(pressured-tray not built — run: cargo build --release --features tray)"
fi

echo "Installing unit   → /etc/systemd/system/rtux.service"
install -m 0644 "$REPO/rtux.service" /etc/systemd/system/rtux.service

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
