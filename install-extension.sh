#!/bin/bash
# install-extension.sh — stage the rtux GNOME Shell extension and arm it to
# auto-enable. Run as YOUR USER (not root):  ./install-extension.sh
#
# The extension does two things only the shell can do: an ambient top-bar
# pressure light, and attention-following (it hands the focused window's PID to
# the daemon so your active app stays resident under pressure — Wayland exposes
# focus to nothing else unprivileged).
#
# IMPORTANT: GNOME on Wayland only *discovers* a newly-added extension at shell
# startup — there is no in-place reload (X11's Alt+F2 'r' is gone). So this
# script stages the files and pre-enables the UUID; the extension comes alive at
# your next login. That's one login whenever you next happen to do one — not an
# extra logout forced now.
set -euo pipefail

UUID="rtux@justinstimatze.com"
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/gnome-extension"
DEST="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/$UUID"

if ! command -v gnome-extensions >/dev/null; then
    echo "note: gnome-extensions not found — this is a GNOME Shell extension and"
    echo "      does nothing on KDE/sway. Staging anyway is harmless."
fi

echo "Staging $UUID → $DEST"
mkdir -p "$DEST"
cp "$SRC"/metadata.json "$SRC"/extension.js "$SRC"/stylesheet.css "$DEST"/

# Pre-arm auto-enable so the extension loads itself at the next login — no manual
# `gnome-extensions enable` needed after logging in.
if command -v gsettings >/dev/null; then
    cur=$(gsettings get org.gnome.shell enabled-extensions)
    if [[ "$cur" != *"$UUID"* ]]; then
        if [[ "$cur" == "@as []" || "$cur" == "[]" ]]; then
            new="['$UUID']"
        else
            new="${cur%]}, '$UUID']"
        fi
        gsettings set org.gnome.shell enabled-extensions "$new"
    fi
    echo "Armed → will auto-enable at next login."
fi

echo
echo "Log in again (a normal reboot counts) and the top-bar dot appears and"
echo "attention-following goes live. No further steps."
echo "(On Xorg you can skip the wait: Alt+F2 → r → Enter reloads the shell now.)"
