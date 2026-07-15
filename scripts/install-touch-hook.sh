#!/usr/bin/env bash
# install-touch-hook.sh — teach Claude Code sessions to tell rtux "a human just
# typed here", so rtux never freezes the pane you're working in.
#
# Why this is needed: under tmux, rtux's foreground-sparing is INOPERATIVE, not
# merely weak. A pane's processes descend from the tmux *server* (which systemd
# parents to `systemd --user`), so the parent chain from the pane never reaches
# the focused terminal window — rtux sees the pane under your fingers as an
# ordinary background hog and freezes it mid-keystroke. Nothing rtux can observe
# from outside fixes this: every tmux client descends from the same single
# terminal-emulator pid (one process, N tabs), and tmux counts *output* as
# activity, so `client_activity` ranks a chatty background agent above the human.
#
# "A human typed here" is a fact only the session has. This hook hands it over:
# on every prompt submit, the session pings rtux's control socket. rtux resolves
# the CALLER's own cgroup from the kernel (SO_PEERCRED) — the ping carries no pid,
# so a session can only ever spare itself. The sparing expires on its own (see
# TOUCH_TTL) and is capped (MAX_LIVE), so an idle session returns to the
# freezable pool and rtux is never starved of candidates.
#
# Usage: scripts/install-touch-hook.sh [--user | --project]
#   --user     install to ~/.claude/settings.json   (all sessions; default)
#   --project  install to ./.claude/settings.json   (this repo only)
set -euo pipefail

SCOPE="${1:---user}"
case "$SCOPE" in
  --user)    TARGET="$HOME/.claude/settings.json" ;;
  --project) TARGET="$(pwd)/.claude/settings.json" ;;
  *) echo "usage: $0 [--user | --project]" >&2; exit 1 ;;
esac

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required to edit settings.json safely." >&2
  exit 1
fi
if ! command -v pressured >/dev/null 2>&1; then
  echo "warning: 'pressured' is not on PATH — the hook will no-op until it is." >&2
fi

# Fire-and-forget: never let a hook failure block a prompt, and never let it
# print anything into the session.
CMD='pressured ctl touch >/dev/null 2>&1 || true'

mkdir -p "$(dirname "$TARGET")"
[ -f "$TARGET" ] || echo '{}' > "$TARGET"

# Idempotent: bail if this exact hook is already present.
if jq -e --arg c "$CMD" '
  (.hooks.UserPromptSubmit // []) | any(.hooks[]?; .command == $c)
' "$TARGET" >/dev/null 2>&1; then
  echo "Already installed in $TARGET — nothing to do."
  exit 0
fi

tmp=$(mktemp)
jq --arg c "$CMD" '
  .hooks //= {} |
  .hooks.UserPromptSubmit //= [] |
  .hooks.UserPromptSubmit += [{"hooks": [{"type": "command", "command": $c}]}]
' "$TARGET" > "$tmp"
mv "$tmp" "$TARGET"

echo "Installed rtux touch hook → $TARGET"
echo
echo "  on each prompt submit:  $CMD"
echo
echo "Takes effect in NEW Claude Code sessions (settings are read at startup)."
echo "Verify by hand in any session:  pressured ctl touch"
echo "  → should print: \"claude · <dir> is live — spared for 300s\""
