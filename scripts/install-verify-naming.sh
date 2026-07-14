#!/usr/bin/env bash
# install-verify-naming.sh — verify the daemon can actually resolve session
# names to their working directory ("claude · rtux", not a bare "claude").
#
# Background: /proc/<pid>/cwd is ptrace-gated. ptrace_may_access() only skips its
# capability check when the reader's creds MATCH the target's — so the root
# daemon reading a uid-1000 process needs CAP_SYS_PTRACE and otherwise gets
# EACCES. The world-readable /proc/<pid>/comm keeps working, so the failure looks
# like a *naming regression* rather than a permissions problem: every session
# degrades to an unnamed "claude".
#
# Run after installing: scripts/install-verify-naming.sh
set -uo pipefail

fail=0
note() { printf '  %s\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$*"; fail=1; }

echo "rtux naming verification"
echo

# --- 1. the daemon must hold CAP_SYS_PTRACE ---
echo "[1] daemon capabilities"
PID=$(systemctl show rtux.service -p MainPID --value 2>/dev/null || echo 0)
if [ -z "$PID" ] || [ "$PID" = "0" ]; then
  bad "rtux.service is not running (no MainPID) — start it first"
else
  note "daemon pid=$PID"
  CAPEFF=$(awk '/^CapEff/{print $2}' "/proc/$PID/status" 2>/dev/null)
  if [ -z "$CAPEFF" ]; then
    bad "could not read CapEff from /proc/$PID/status"
  elif python3 -c "import sys; sys.exit(0 if int('$CAPEFF',16) & (1<<19) else 1)"; then
    ok "CAP_SYS_PTRACE present in CapEff ($CAPEFF) — /proc/<pid>/cwd is readable"
  else
    bad "CAP_SYS_PTRACE MISSING from CapEff ($CAPEFF)"
    note "  → session labels will silently degrade to a bare \"claude\"."
    note "  → fix: CapabilityBoundingSet in rtux.service must include CAP_SYS_PTRACE,"
    note "    then: sudo systemctl daemon-reload && sudo systemctl restart rtux.service"
  fi
fi
echo

# --- 2. the labels the daemon actually resolves ---
echo "[2] resolved session labels (via the daemon's own ctl list)"
if ! command -v pressured >/dev/null 2>&1; then
  bad "pressured not on PATH — cannot query the daemon"
else
  labels=$(pressured ctl list 2>/dev/null | grep -E 'spawn-' || true)
  if [ -z "$labels" ]; then
    note "no terminal-scope sessions above the list floor right now — nothing to check."
    note "(start a claude session and re-run, or trust check [1].)"
  else
    named=$(printf '%s\n' "$labels" | grep -c '·' || true)
    bare=$(printf '%s\n' "$labels" | grep -cE '(^|[[:space:]])claude[[:space:]]{2,}' || true)
    printf '%s\n' "$labels" | awk '{printf "      %s\n", $4}' | sort -u | head -12
    echo
    if [ "$named" -gt 0 ]; then
      ok "$named session(s) resolved WITH a directory (\"claude · dir\")"
    fi
    if [ "$bare" -gt 0 ] && [ "$named" -eq 0 ]; then
      bad "all sessions show a bare name — cwd is not resolving (see check [1])"
    fi
  fi
fi
echo

if [ "$fail" -eq 0 ]; then
  echo "PASS — the daemon can name sessions by directory."
else
  echo "FAIL — see the notes above."
fi
exit "$fail"
