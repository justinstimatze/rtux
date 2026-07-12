#!/bin/bash
# uninstall.sh — reverse everything install.sh / setup-zram.sh / setup-hotkey.sh
# / install-extension.sh put in place. Run as root:  sudo ./uninstall.sh
#
# Deliberately NOT `set -e`: we want to remove as much as possible even if one
# step (say, a per-user gsettings call) fails.
set -uo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "error: run as root:  sudo ./uninstall.sh"
    exit 1
fi

USER_NAME="${SUDO_USER:-}"

# Run a command inside the graphical user's session (for gsettings, which needs
# their D-Bus session bus).
run_user() {
    [[ -n "$USER_NAME" ]] || return 0
    local uid
    uid=$(id -u "$USER_NAME")
    sudo -u "$USER_NAME" \
        DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" "$@"
}

# Drop an entry from a gsettings string-array, handling only/first/middle/last.
strip_from_list() {
    sed -e "s#'$1', ##" -e "s#, '$1'##" -e "s#'$1'##"
}

echo "== stopping + removing the service =="
systemctl disable --now rtux.service 2>/dev/null
rm -f /etc/systemd/system/rtux.service
systemctl daemon-reload
systemctl reset-failed rtux.service 2>/dev/null || true

echo "== removing binaries + desktop file =="
rm -f /usr/local/bin/pressured /usr/local/bin/pressured-hud /usr/local/bin/pressured-tray
rm -f /usr/share/applications/dev.pressured.Hud.desktop

if [[ -n "$USER_NAME" ]]; then
    UHOME=$(getent passwd "$USER_NAME" | cut -d: -f6)
    echo "== removing per-user bits for $USER_NAME =="
    rm -f "$UHOME/.config/autostart/pressured-tray.desktop"

    # Hotkey binding
    SCHEMA="org.gnome.settings-daemon.plugins.media-keys"
    CK="/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/pressured/"
    cur=$(run_user gsettings get "$SCHEMA" custom-keybindings 2>/dev/null || echo "")
    if [[ "$cur" == *"$CK"* ]]; then
        new=$(printf '%s' "$cur" | strip_from_list "$CK")
        run_user gsettings set "$SCHEMA" custom-keybindings "$new"
        echo "  removed Ctrl+Alt+P binding"
    fi

    # GNOME Shell extension (staged files + enabled list)
    UUID="rtux@justinstimatze.com"
    ecur=$(run_user gsettings get org.gnome.shell enabled-extensions 2>/dev/null || echo "")
    if [[ "$ecur" == *"$UUID"* ]]; then
        enew=$(printf '%s' "$ecur" | strip_from_list "$UUID")
        run_user gsettings set org.gnome.shell enabled-extensions "$enew"
    fi
    rm -rf "$UHOME/.local/share/gnome-shell/extensions/$UUID"
    echo "  removed the top-bar extension (gone at next login)"
fi

echo "== zram =="
if swapon --show=NAME --noheadings 2>/dev/null | grep -q '^/dev/zram'; then
    swapoff /dev/zram0 2>/dev/null || true
fi
rm -f /etc/systemd/zram-generator.conf /etc/sysctl.d/99-zram.conf
systemctl daemon-reload
echo "  removed zram config + tuning (the package is left installed —"
echo "  remove it with your package manager if you like)"

echo
echo "Done. rtux is removed."
echo "Note: any memory.min limits the daemon set on cgroups clear on the next reboot."
