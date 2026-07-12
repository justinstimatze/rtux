#!/bin/bash
# setup-hotkey.sh — bind a key to summon the pressured HUD (GNOME).
# Run as YOUR USER (not root):  ./setup-hotkey.sh
# Default binding: Ctrl+Alt+P. Override: ./setup-hotkey.sh '<Super>grave'
# NOTE: avoid plain <Super>+<key> combos — GNOME/Mutter grabs most of them for
# the shell, so the keypress never reaches a custom keybinding.
set -euo pipefail

BINDING="${1:-<Control><Alt>p}"
SCHEMA="org.gnome.settings-daemon.plugins.media-keys"
KEYPATH="/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/pressured/"

if ! command -v gsettings >/dev/null; then
    echo "error: gsettings not found — this script targets GNOME. On KDE/sway, bind"
    echo "       'pressured-hud' to a shortcut via your DE's keyboard settings."
    exit 1
fi

if ! command -v pressured-hud >/dev/null; then
    echo "warning: 'pressured-hud' isn't on PATH — the hotkey would launch nothing."
    echo "         Build + install it first:"
    echo "           cargo build --release --features hud,tray && sudo ./install.sh"
    echo "         Binding anyway; it'll work once the HUD is installed."
fi

# Append our keybinding path to the existing list without clobbering others.
current=$(gsettings get "$SCHEMA" custom-keybindings)
if [[ "$current" != *"$KEYPATH"* ]]; then
    if [[ "$current" == "@as []" || "$current" == "[]" ]]; then
        new="['$KEYPATH']"
    else
        new="${current%]}, '$KEYPATH']"
    fi
    gsettings set "$SCHEMA" custom-keybindings "$new"
fi

CK="$SCHEMA.custom-keybinding:$KEYPATH"
gsettings set "$CK" name 'pressured HUD'
# New-client-per-summon. On Wayland the compositor only grants focus to a *fresh*
# client; an already-running app re-asking (present()/activate) is denied by
# focus-stealing-prevention, and GNOME hands custom keybindings no activation
# token to override that. So kill any running HUD, wait for it to release the bus
# name, then launch a new process — which reliably jumps to the front. The HUD is
# a stateless live view, so respawning is free. SIGKILL (not TERM) so the old
# process dies *now* and frees the D-Bus name before we relaunch — otherwise the
# new process races the dying one, becomes its remote, and the summon flaps.
gsettings set "$CK" command "sh -c 'pkill -KILL -x pressured-hud; for i in \$(seq 50); do pgrep -x pressured-hud >/dev/null || break; sleep 0.02; done; pressured-hud'"
gsettings set "$CK" binding "$BINDING"

echo "Bound $BINDING → pressured-hud"
echo "Press it to summon the HUD (re-pressing raises the existing window)."
